<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { rangesToChoices } from '$lib/controls/optionsModel';
  import Nixie from '$lib/controls/Nixie.svelte';
  import LiveControls from '$lib/controls/LiveControls.svelte';
  import FftControls from './FftControls.svelte';

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

  // The FFT is tapped directly off the source (standard SDR UX — see
  // wbfm.json), so the renderer labels match `currentAxes`. Clicks
  // inside the wide view move the VFO (orange marker) to the click
  // location; the source itself stays put unless there's no VFO.
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

  // Sample-rate dropdown. Hardware sources give us explicit rate
  // ladders via sourceCaps; software sources (sine, file) don't
  // advertise any, so we fall back to a read-only display. The current
  // rate is always included — devices sometimes report ranges that
  // don't cover what the preset happens to ship with.
  let rateChoices = $derived.by(() => {
    const caps = pipeline.sourceCaps;
    if (!caps || caps.kind !== 'hardware') return [] as number[];
    const ch = caps.capabilities.rx_channels[0];
    if (!ch) return [] as number[];
    const choices = rangesToChoices(ch.sample_rate_ranges_hz);
    const rate = axes?.sample_rate_hz;
    if (rate !== undefined && !choices.some((c) => Math.abs(c - rate) < 1)) {
      choices.push(rate);
      choices.sort((a, b) => a - b);
    }
    return choices;
  });

  function fmtRate(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MS/s`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(1)} kS/s`;
    return `${hz} S/s`;
  }

  function onRateChange(ev: Event) {
    const v = Number((ev.target as HTMLSelectElement).value);
    if (!Number.isFinite(v) || v === axes?.sample_rate_hz) return;
    void pipeline.patchSourceParams({ sample_rate_hz: v });
  }

  function commitCenter(hz: number) {
    if (axes && hz !== axes.center_freq_hz) {
      void pipeline.patchSourceParams({ center_freq_hz: hz });
    }
  }

  // Committing the VFO writes the offset (`freq_shift_hz = target −
  // center`) so the absolute listening frequency matches what the user
  // typed. Clamp to the source span so the UI doesn't silently tune
  // outside what the channelizer can reach.
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
    if (!renderer || !canvas) return;
    const rect = canvas.getBoundingClientRect();
    const f = renderer.pixelToFreq(ev.clientX - rect.left);
    if (f === undefined) return;
    // Click moves the VFO inside the wide RF view — source stays
    // tuned. Without a channelizer block (bareband presets), click
    // re-tunes the source itself.
    if (vfoBlock && axes) {
      commitVfo(Math.round(f));
    } else {
      void pipeline.patchSourceParams({ center_freq_hz: Math.round(f) });
    }
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
  // Floor/ceil track the live logmag block's params so adjusting them
  // via the FFT controls strip updates the y-axis labels in lockstep.
  const DEFAULT_FLOOR_DBFS = -100;
  const DEFAULT_CEIL_DBFS = 0;
  let logmagValues = $derived(
    (pipeline.blocks.logmag?.values as Record<string, unknown> | null | undefined) ?? null,
  );
  let floorDbfs = $derived(
    typeof logmagValues?.floor_dbfs === 'number' ? logmagValues.floor_dbfs : DEFAULT_FLOOR_DBFS,
  );
  let ceilDbfs = $derived(
    typeof logmagValues?.ceil_dbfs === 'number' ? logmagValues.ceil_dbfs : DEFAULT_CEIL_DBFS,
  );
  $effect(() => {
    if (!renderer) return;
    if (!axes) {
      renderer.setAxes(undefined);
      return;
    }
    renderer.setAxes({
      centerHz: axes.center_freq_hz,
      rateHz: axes.sample_rate_hz,
      floorDbfs,
      ceilDbfs,
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

      <label class="flex items-center gap-1" title="sample rate / span">
        <span>rate</span>
        {#if rateChoices.length > 1}
          <select
            class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
            value={axes.sample_rate_hz}
            onchange={onRateChange}
          >
            {#each rateChoices as r (r)}
              <option value={r}>{fmtRate(r)}</option>
            {/each}
          </select>
        {:else}
          <span class="font-mono text-slate-300">{fmtRate(axes.sample_rate_hz)}</span>
        {/if}
      </label>

      <LiveControls />

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
    <FftControls />
  {/if}
  <canvas
    bind:this={canvas}
    onclick={onClick}
    onwheel={onWheel}
    class="block min-h-0 w-full flex-1 cursor-crosshair"
    title="click to tune · wheel to nudge by step"
  ></canvas>
</div>
