<script lang="ts">
  // Companion to `NarrowWaterfall.svelte` that paints the line-plot
  // spectrum of the channelizer's output. Subscribes to the runtime-
  // injected `ui:fft_narrow` sink (see `inject_narrow_fft.rs`), derives
  // axes from `source_center + freq_shift_hz ± channelizer.output_rate_hz/2`,
  // and reuses the same `SpectrumRenderer` as the wide pane. No VFO
  // marker (the narrow view IS centred on the VFO), no zoom/pan/click
  // controls — read-only at this stage.
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();

  let fftStreamId = $derived(pipeline.uiSinks.fft_narrow?.stream_id);

  // VFO position (lifted from the channelizer's freq_shift_hz) +
  // channelizer output_rate define the narrow view's axes.
  let wideAxes = $derived(currentAxes(pipeline));
  let vfoBlock = $derived(
    Object.values(pipeline.blocks).find((b) =>
      b.spec.params.some((p) => p.key === 'freq_shift_hz'),
    ),
  );
  let vfoValues = $derived(
    (vfoBlock?.values as Record<string, unknown> | null | undefined) ?? null,
  );
  let vfoShiftHz = $derived(
    typeof vfoValues?.freq_shift_hz === 'number' ? vfoValues.freq_shift_hz : 0,
  );
  let narrowCenterHz = $derived(wideAxes ? wideAxes.center_freq_hz + vfoShiftHz : undefined);
  let narrowRateHz = $derived.by(() => {
    if (!vfoValues || !wideAxes) return undefined;
    const outRate = vfoValues.output_rate_hz;
    if (typeof outRate === 'number' && outRate > 0) return outRate;
    const factor = vfoValues.factor;
    if (typeof factor === 'number' && factor > 0) return wideAxes.sample_rate_hz / factor;
    return undefined;
  });

  let canvas: HTMLCanvasElement | undefined = $state();
  let renderer: SpectrumRenderer | undefined;

  // Same display-range knobs as the wide spectrum — operator can dial
  // them once and both panes track. Auto-scale is shared too.
  let autoScale = $derived(clientControls.get('client.spectrum.autoScale'));
  let manualFloor = $derived(clientControls.get('client.spectrum.displayFloorDbfs'));
  let manualCeil = $derived(clientControls.get('client.spectrum.displayCeilDbfs'));
  let fade = $derived(clientControls.get('client.spectrum.fade'));
  let maxHold = $derived(clientControls.get('client.spectrum.maxHold'));

  const SERVER_FLOOR_DBFS = -160;
  const SERVER_CEIL_DBFS = 0;

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

  $effect(() => {
    if (!renderer) return;
    if (narrowCenterHz === undefined || narrowRateHz === undefined || narrowRateHz <= 0) {
      renderer.setAxes(undefined);
      return;
    }
    renderer.setAxes({
      centerHz: narrowCenterHz,
      rateHz: narrowRateHz,
      floorDbfs: SERVER_FLOOR_DBFS,
      ceilDbfs: SERVER_CEIL_DBFS,
    });
  });

  $effect(() => {
    if (!renderer || autoScale) return;
    renderer.setDisplayRange({ floorDbfs: manualFloor, ceilDbfs: manualCeil });
  });

  $effect(() => {
    renderer?.setFeatures({ fade, maxHold });
  });
</script>

{#if fftStreamId !== undefined}
  <canvas bind:this={canvas} class="block h-full w-full"></canvas>
{:else}
  <div
    class="flex h-full w-full items-center justify-center text-[11px] text-[color:var(--color-muted)]"
  >
    no channel — preset has no channelizer
  </div>
{/if}
