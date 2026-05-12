<script lang="ts">
  import { onMount } from 'svelte';
  import { pixelToFreqLinear, WaterfallRenderer } from './waterfall';
  import { LEFT_MARGIN, RIGHT_MARGIN } from './spectrum';
  import { waterfallStore } from './waterfallStore.svelte';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';

  interface Props {
    client: FrameClient;
    rows?: number;
  }

  let { client, rows = 512 }: Props = $props();

  let fftStreamId = $derived(pipeline.uiSinks.fft?.stream_id);

  let canvas: HTMLCanvasElement | undefined = $state();
  let wrap: HTMLDivElement | undefined = $state();

  let axes = $derived(currentAxes(pipeline));

  // Same view-window derivation the spectrum uses — both panes stay
  // locked together so a zoom/pan affects them as a single viewport.
  let viewZoom = $derived(clientControls.get('client.spectrum.viewZoom'));
  let viewPan = $derived(clientControls.get('client.spectrum.viewPan'));
  let viewWindow = $derived.by(() => {
    if (!axes) return undefined;
    const z = Math.max(1, viewZoom);
    if (z <= 1) return undefined;
    const span = axes.sample_rate_hz / z;
    const headroom = axes.sample_rate_hz - span;
    const fullMin = axes.center_freq_hz - axes.sample_rate_hz / 2;
    const viewMin = fullMin + Math.max(0, Math.min(1, viewPan)) * headroom;
    return { centerHz: viewMin + span / 2, rateHz: span };
  });

  // VFO marker (orange line + translucent band) — same idea as the
  // spectrum's `drawMarkers`, but rendered as HTML overlay divs because
  // the waterfall canvas is GL and we don't want to round-trip the
  // marker through the data texture. Block discovery mirrors
  // Spectrum.svelte: any block with a `freq_shift_hz` param is the
  // VFO; absent that knob the preset is bareband and we draw nothing.
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
  let vfoAbsHz = $derived(axes ? axes.center_freq_hz + vfoShiftHz : 0);
  let vfoWidthHz = $derived.by(() => {
    if (!vfoValues || !axes) return undefined;
    const outRate = vfoValues.output_rate_hz;
    if (typeof outRate === 'number' && outRate > 0) return outRate;
    const factor = vfoValues.factor;
    if (typeof factor === 'number' && factor > 0) return axes.sample_rate_hz / factor;
    return undefined;
  });

  // CSS percentages within the canvas wrapper (which already pads
  // LEFT_MARGIN/RIGHT_MARGIN). Returns `null` when nothing should be
  // drawn so the markup can `{#if}`-guard the overlays cleanly.
  let vfoCssPct = $derived.by(() => {
    if (!axes || !vfoBlock) return null;
    const cHz = viewWindow?.centerHz ?? axes.center_freq_hz;
    const rHz = viewWindow?.rateHz ?? axes.sample_rate_hz;
    const fMin = cHz - rHz / 2;
    const fMax = cHz + rHz / 2;
    if (vfoAbsHz < fMin || vfoAbsHz > fMax) return null;
    const center = ((vfoAbsHz - fMin) / rHz) * 100;
    let leftPct: number | null = null;
    let widthPct: number | null = null;
    if (vfoWidthHz && vfoWidthHz > 0) {
      const lo = Math.max(fMin, vfoAbsHz - vfoWidthHz / 2);
      const hi = Math.min(fMax, vfoAbsHz + vfoWidthHz / 2);
      leftPct = ((lo - fMin) / rHz) * 100;
      widthPct = ((hi - lo) / rHz) * 100;
    }
    return { center, leftPct, widthPct };
  });

  // Pointer drag state: `dragX` is the current pointer CSS-X relative to
  // the canvas. While null the cursor sits at geometric centre — the VFO
  // already lives at `center_freq_hz`, which the waterfall re-centres on.
  let dragging = $state(false);
  let dragX = $state<number | null>(null);

  function pointerFreq(clientX: number): number | null {
    if (!canvas || !axes) return null;
    const rect = canvas.getBoundingClientRect();
    // When zoomed/panned the canvas paints only a sub-range of the full
    // span; click-to-tune has to project against the *visible* window,
    // not the full axes, or the cursor lands tens of MHz off.
    const cHz = viewWindow?.centerHz ?? axes.center_freq_hz;
    const rHz = viewWindow?.rateHz ?? axes.sample_rate_hz;
    return pixelToFreqLinear(clientX - rect.left, rect.width, cHz, rHz);
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
    if (f !== null) void applyControl('flow.src.center_freq_hz', Math.round(f));
  }

  function onPointerCancel(ev: PointerEvent) {
    if (!dragging) return;
    (ev.currentTarget as HTMLElement).releasePointerCapture(ev.pointerId);
    dragging = false;
    dragX = null;
  }

  let renderer: WaterfallRenderer | undefined;

  // Waterfall contrast knobs — auto-track P5/P98 of the byte stream by
  // default; manual override expressed in dBFS to match the spectrum
  // pane's slider. SERVER_FLOOR_DBFS / SERVER_CEIL_DBFS are the same
  // -160..0 window LogMagU8 quantises to (see blocks/src/log_mag_u8.rs),
  // so a dBFS value maps to a normalised byte by (dBFS + 160) / 160.
  const WF_FLOOR_DBFS = -160;
  const WF_CEIL_DBFS = 0;
  function dbfsToByte01(d: number): number {
    return (d - WF_FLOOR_DBFS) / (WF_CEIL_DBFS - WF_FLOOR_DBFS);
  }
  let autoContrast = $derived(clientControls.get('client.waterfall.autoContrast'));
  let floorDbfs = $derived(clientControls.get('client.waterfall.contrastFloorDbfs'));
  let ceilDbfs = $derived(clientControls.get('client.waterfall.contrastCeilDbfs'));

  onMount(() => {
    if (!canvas) return;
    renderer = new WaterfallRenderer(canvas, { rows });
    // Push the persisted contrast state into the renderer on mount
    // — derived effects run later, but mount-time state needs to be
    // applied before the first row uploads so the first paint isn't
    // a flash of unstyled palette.
    renderer.setAutoContrast(autoContrast);
    renderer.setManualContrast(dbfsToByte01(floorDbfs), dbfsToByte01(ceilDbfs));
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);
    return () => {
      ro.disconnect();
      renderer?.destroy();
      renderer = undefined;
    };
  });

  // Sync contrast settings into the renderer reactively.
  $effect(() => {
    if (!renderer) return;
    renderer.setAutoContrast(autoContrast);
    renderer.setManualContrast(dbfsToByte01(floorDbfs), dbfsToByte01(ceilDbfs));
  });

  $effect(() => {
    const sid = fftStreamId;
    if (sid === undefined) return;
    const unsub = client.subscribe(sid, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      if (waterfallStore.paused) return;
      renderer?.pushRow(frame.payload);
    });
    return unsub;
  });

  $effect(() => {
    if (!renderer || !axes) return;
    if (!viewWindow) {
      renderer.setView(undefined);
      return;
    }
    renderer.setView({
      centerHz: viewWindow.centerHz,
      rateHz: viewWindow.rateHz,
      fullCenterHz: axes.center_freq_hz,
      fullRateHz: axes.sample_rate_hz,
    });
  });
</script>

<div bind:this={wrap} class="relative flex h-full w-full flex-col">
  <div
    class="relative min-h-0 flex-1"
    style:padding-left="{LEFT_MARGIN}px"
    style:padding-right="{RIGHT_MARGIN}px"
  >
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
        style:left={dragX != null
          ? `${LEFT_MARGIN + dragX}px`
          : `calc(${LEFT_MARGIN}px + (100% - ${LEFT_MARGIN + RIGHT_MARGIN}px) / 2)`}
        style:box-shadow={dragging
          ? '0 0 4px rgba(251, 191, 36, 0.8)'
          : '0 0 3px rgba(56, 189, 248, 0.6)'}
      ></div>
    {/if}
    {#if vfoCssPct}
      {#if vfoCssPct.leftPct != null && vfoCssPct.widthPct != null && vfoCssPct.widthPct > 0}
        <!-- Translucent VFO bandwidth band, sized to the channel filter. -->
        <div
          class="pointer-events-none absolute top-0 bottom-0"
          style:left="calc({LEFT_MARGIN}px + (100% - {LEFT_MARGIN + RIGHT_MARGIN}px) * {vfoCssPct.leftPct}
          / 100)"
          style:width="calc((100% - {LEFT_MARGIN + RIGHT_MARGIN}px) * {vfoCssPct.widthPct} / 100)"
          style:background="rgba(255, 157, 58, 0.12)"
          style:border-left="1px solid rgba(255, 157, 58, 0.4)"
          style:border-right="1px solid rgba(255, 157, 58, 0.4)"
        ></div>
      {/if}
      <!-- VFO centre line. Sits over the band so it stays visible even
           when the band is narrow enough to be a single pixel wide. -->
      <div
        class="pointer-events-none absolute top-0 bottom-0 w-px bg-orange-400"
        style:left="calc({LEFT_MARGIN}px + (100% - {LEFT_MARGIN + RIGHT_MARGIN}px) * {vfoCssPct.center}
        / 100)"
        style:box-shadow="0 0 4px rgba(255, 157, 58, 0.6)"
      ></div>
    {/if}
  </div>
</div>
